//! whereti-service.exe — the Windows service that keeps the index current
//! and answers searches over a named pipe (PLAN.md §3, §8).
//!
//!   whereti-service install     register (automatic start, restart on failure)
//!   whereti-service uninstall   stop and remove
//!   whereti-service start|stop
//!   whereti-service console     run in the foreground (testing)
//!   whereti-service run         entry point used by the Service Control Manager
//!
//! Everything except `run` needs an Administrator terminal.

#![cfg(windows)]

use std::ffi::OsString;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use whereti_engine::{system, Config, Engine, Logger};
use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept, ServiceErrorControl,
    ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState,
    ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

const SERVICE_NAME: &str = "whereti";
const DISPLAY_NAME: &str = "whereti search index";
const DESCRIPTION: &str =
    "Keeps the whereti file index up to date from the NTFS change journal and answers searches. Local only, no network.";

define_windows_service!(ffi_service_main, service_main);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("run") => service_dispatcher::start(SERVICE_NAME, ffi_service_main).map_err(|e| e.to_string()),
        Some("console") => {
            let seconds = flag_value(&args, "--seconds").and_then(|s| s.parse().ok());
            let cfg = Config { verbose: args.iter().any(|a| a == "--verbose"), ..Config::default() };
            whereti_engine::run_foreground(cfg, seconds)
        }
        Some("install") => install(),
        Some("uninstall") => uninstall(),
        Some("start") => control(true),
        Some("stop") => control(false),
        _ => {
            eprintln!("usage: whereti-service install | uninstall | start | stop | console [--seconds N] [--verbose]");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn flag_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = run_service() {
        if let Ok(log) = open_log() {
            log.log(format!("service failed: {e}"));
        }
    }
}

fn open_log() -> Result<Logger, String> {
    let dir = system::ensure_data_dir().map_err(|e| e.to_string())?;
    Logger::file(&dir.join("service.log"), false).map_err(|e| e.to_string())
}

fn run_service() -> Result<(), String> {
    let (stop_tx, stop_rx) = mpsc::channel();
    let handler = move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = stop_tx.send(());
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let status = service_control_handler::register(SERVICE_NAME, handler).map_err(|e| e.to_string())?;
    let set = |state, accept, wait: u64| {
        let _ = status.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accept,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::from_secs(wait),
            process_id: None,
        });
    };
    set(ServiceState::StartPending, ServiceControlAccept::empty(), 10);

    let log = open_log()?;
    log.log(format!("whereti service {} starting", env!("CARGO_PKG_VERSION")));
    let engine = Engine::start(Config::default(), log);
    let pipe_handler: whereti_ipc::Handler = {
        let engine = engine.clone();
        Arc::new(move |req| engine.handle(req))
    };
    let server = match whereti_ipc::Server::start(pipe_handler) {
        Ok(s) => s,
        Err(e) => {
            engine.log(format!("cannot open {}: {e}", whereti_ipc::PIPE_NAME));
            engine.stop();
            set(ServiceState::Stopped, ServiceControlAccept::empty(), 0);
            return Err(e.to_string());
        }
    };
    set(ServiceState::Running, ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN, 0);

    let _ = stop_rx.recv();
    set(ServiceState::StopPending, ServiceControlAccept::empty(), 20);
    server.stop();
    engine.stop(); // saves the index if it changed
    set(ServiceState::Stopped, ServiceControlAccept::empty(), 0);
    Ok(())
}

fn install() -> Result<(), String> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)
        .map_err(|e| format!("{e} (run from an Administrator terminal)"))?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: DISPLAY_NAME.into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe,
        launch_arguments: vec!["run".into()],
        dependencies: vec![],
        account_name: None, // LocalSystem: needed to read the volumes
        account_password: None,
    };
    let service = manager
        .create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)
        .map_err(|e| e.to_string())?;
    service.set_description(DESCRIPTION).map_err(|e| e.to_string())?;
    // Restart after 5 s, 30 s, 60 s; forget failures after a day (PLAN.md §8).
    service
        .update_failure_actions(ServiceFailureActions {
            reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 3600)),
            reboot_msg: None,
            command: None,
            actions: Some(vec![
                ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(5) },
                ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(30) },
                ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(60) },
            ]),
        })
        .map_err(|e| e.to_string())?;
    service.set_failure_actions_on_non_crash_failures(true).map_err(|e| e.to_string())?;
    println!("installed the \"{SERVICE_NAME}\" service (automatic start); run `whereti-service start` to start it now");
    Ok(())
}

fn uninstall() -> Result<(), String> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|e| format!("{e} (run from an Administrator terminal)"))?;
    let service = manager
        .open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE)
        .map_err(|e| e.to_string())?;
    if service.query_status().map_err(|e| e.to_string())?.current_state != ServiceState::Stopped {
        let _ = service.stop();
        for _ in 0..100 {
            if service.query_status().map(|s| s.current_state == ServiceState::Stopped).unwrap_or(true) {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    service.delete().map_err(|e| e.to_string())?;
    println!("removed the \"{SERVICE_NAME}\" service (the index in {} is kept)", system::data_dir().display());
    Ok(())
}

fn control(start: bool) -> Result<(), String> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|e| format!("{e} (run from an Administrator terminal)"))?;
    let service = manager
        .open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::START | ServiceAccess::STOP)
        .map_err(|e| e.to_string())?;
    if start {
        service.start::<&str>(&[]).map_err(|e| e.to_string())?;
        println!("started");
    } else {
        service.stop().map_err(|e| e.to_string())?;
        println!("stopping (the index is saved first)");
    }
    Ok(())
}
