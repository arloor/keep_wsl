use std::{
    default,
    env::args,
    ffi::OsString,
    future::Future,
    io,
    process::ExitCode,
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};

use clap::{Arg, ArgMatches, Command};
use futures_util::future::{self, Either};
use log::{error, info, warn};
use tokio::{
    runtime::{Builder, Runtime},
    sync::oneshot,
};
use windows_service::{
    define_windows_service,
    service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult, ServiceStatusHandle},
    service_dispatcher,
};

const SERVICE_NAME: &str = "keep_WSL";
const SERVICE_EXIT_CODE_ARGUMENT_ERROR: u32 = 100;
const SERVICE_EXIT_CODE_EXITED_UNEXPECTLY: u32 = 101;
const SERVICE_EXIT_CODE_CREATE_FAILED: u32 = 102;

#[inline]
fn set_service_status(
    handle: &ServiceStatusHandle,
    current_state: ServiceState,
    exit_code: ServiceExitCode,
    wait_hint: Duration,
) -> Result<(), windows_service::Error> {
    static SERVICE_STATE_CHECKPOINT: AtomicU32 = AtomicU32::new(0);

    let next_status = ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state,
        controls_accepted: if current_state == ServiceState::StartPending {
            ServiceControlAccept::empty()
        } else {
            ServiceControlAccept::STOP
        },
        exit_code,
        checkpoint: if matches!(current_state, ServiceState::Running | ServiceState::Stopped) {
            SERVICE_STATE_CHECKPOINT.fetch_add(1, Ordering::AcqRel)
        } else {
            0
        },
        wait_hint,
        process_id: None,
    };
    handle.set_service_status(next_status)
}

fn handle_create_service_result<F>(
    status_handle: ServiceStatusHandle,
    create_service_result: Result<(Runtime, F), ExitCode>,
    stop_receiver: oneshot::Receiver<()>,
) -> Result<(), windows_service::Error>
where
    F: Future<Output = ExitCode>,
{
    match create_service_result {
        Ok((runtime, main_fut)) => {
            // Successfully create runtime and future

            // Report running state
            set_service_status(
                &status_handle,
                ServiceState::Running,
                ServiceExitCode::Win32(0),
                Duration::default(),
            )?;

            // Run it right now.
            let exited_by_ctrl = runtime.block_on(async move {
                tokio::pin!(main_fut);
                tokio::pin!(stop_receiver);

                tokio::select! {
                    _ = stop_receiver => {
                        true
                    }
                    exit_code = main_fut => {
                        info!("service exited unexpectly with code: {:?}", exit_code);
                        false
                    }
                }
            });

            // Report stopped state
            set_service_status(
                &status_handle,
                ServiceState::Stopped,
                if exited_by_ctrl {
                    ServiceExitCode::Win32(0)
                } else {
                    ServiceExitCode::ServiceSpecific(SERVICE_EXIT_CODE_EXITED_UNEXPECTLY)
                },
                Duration::default(),
            )?;
        }
        Err(exit_code) => {
            error!("failed to create service, exit code: {:?}", exit_code);

            // Report running state
            set_service_status(
                &status_handle,
                ServiceState::Stopped,
                ServiceExitCode::ServiceSpecific(SERVICE_EXIT_CODE_CREATE_FAILED),
                Duration::default(),
            )?;
        }
    }

    Ok(())
}

fn service_main(arguments: Vec<OsString>) -> Result<(), windows_service::Error> {
    // Create a Oneshot channel for receiving Stop event
    let (stop_sender, stop_receiver) = oneshot::channel();

    let mut stop_sender_opt = Some(stop_sender);
    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            ServiceControl::Stop => {
                if let Some(stop_sender) = stop_sender_opt.take() {
                    let _ = stop_sender.send(());
                }
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    // Register system service event handler
    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;

    // Report SERVICE_START_PENDING
    // https://learn.microsoft.com/en-us/windows/win32/services/writing-a-servicemain-function
    set_service_status(
        &status_handle,
        ServiceState::StartPending,
        ServiceExitCode::Win32(0),
        Duration::from_secs(30),
    )?;

    let app = Command::new("shadowsocks service")
        .version("0.1.0")
        .about("keep wsl running");

    let app = app.arg(
        Arg::new("debug")
            .short('d')
            .default_value("false")
            .help("turns on debugging mode"),
    );

    let matches_result = if arguments.len() <= 1 {
        app.try_get_matches()
    } else {
        app.try_get_matches_from(arguments)
    };

    let matches = match matches_result {
        Ok(m) => m,
        Err(err) => {
            error!("failed to parse command line arguments, error: {}", err);
            set_service_status(
                &status_handle,
                ServiceState::Stopped,
                ServiceExitCode::ServiceSpecific(SERVICE_EXIT_CODE_ARGUMENT_ERROR),
                Duration::default(),
            )?;
            return Err(windows_service::Error::LaunchArgumentsNotSupported);
        }
    };

    handle_create_service_result(status_handle, create(&matches), stop_receiver)
}

fn service_entry(arguments: Vec<OsString>) {
    if let Err(err) = service_main(arguments) {
        error!("service main exited with error: {}", err);
    }
}

define_windows_service!(ffi_service_entry, service_entry);

fn main() -> Result<(), windows_service::Error> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_entry)?;
    Ok(())
}

/// Create `Runtime` and `main` entry
pub fn create(matches: &ArgMatches) -> Result<(Runtime, impl Future<Output = ExitCode>), ExitCode> {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("create tokio Runtime");

    let main_fut = async move {
        let abort_signal = create_signal_monitor();
        let server = async {
            let result = std::process::Command::new(r"C:\Users\arloor\Desktop\keep.bat").output();
            match result {
                Ok(output) => {
                    let str = String::from_utf8(output.stdout)
                        .unwrap_or("unknown".to_string())
                        .trim()
                        .to_owned();
                    info!("keep.bat: {}", str);
                }
                Err(e) => {
                    warn!("keep.bat error: {}", e);
                }
            }
            Ok::<(), io::Error>(())
        };

        tokio::pin!(abort_signal);
        tokio::pin!(server);

        match future::select(server, abort_signal).await {
            // Server future resolved without an error. This should never happen.
            Either::Left((Ok(..), ..)) => {
                eprintln!("server exited unexpectedly");
                crate::EXIT_CODE_SERVER_EXIT_UNEXPECTEDLY.into()
            }
            // Server future resolved with error, which are listener errors in most cases
            Either::Left((Err(err), ..)) => {
                eprintln!("server aborted with {err}");
                crate::EXIT_CODE_SERVER_ABORTED.into()
            }
            // The abort signal future resolved. Means we should just exit.
            Either::Right(_) => ExitCode::SUCCESS,
        }
    };

    Ok((runtime, main_fut))
}

pub async fn create_signal_monitor() -> io::Result<()> {
    let _ = tokio::signal::ctrl_c().await;

    info!("received {}, exiting", "ctrl_c");

    Ok(())
}

pub const EXIT_CODE_SERVER_EXIT_UNEXPECTEDLY: sysexits::ExitCode = sysexits::ExitCode::Software;
/// Exit code when server aborted
pub const EXIT_CODE_SERVER_ABORTED: sysexits::ExitCode = sysexits::ExitCode::Software;
