use crate::{EVENTS_BUS, FILESYSTEM_LAYOUT, SERVER_DATA_MANAGER};
use alvr_common::{log::LevelFilter, LogEntry, LogSeverity};
use alvr_events::{Event, EventType};
use chrono::Local;
use fern::Dispatch;
use std::fs::OpenOptions;

pub fn init_logging() {
    // NB: using crossbeam channel instead of std channel because we need it to be Sync:
    // better performance because we don't require a mutex
    let mut log_dispatch = Dispatch::new().format(move |out, message, record| {
        let maybe_event = format!("{message}");
        let event_type = if maybe_event.starts_with('{') && maybe_event.ends_with('}') {
            serde_json::from_str(&maybe_event).unwrap()
        } else {
            EventType::Log(LogEntry {
                severity: LogSeverity::from_log_level(record.level()),
                content: message.to_string(),
            })
        };
        let event = Event {
            timestamp: Local::now().format("%H:%M:%S.%f").to_string(),
            event_type,
        };
        out.finish(format_args!("{}", serde_json::to_string(&event).unwrap()));

        if let Some(bus) = &mut *EVENTS_BUS.lock() {
            bus.try_broadcast(event).ok();
        }
    });

    if cfg!(debug_assertions) {
        log_dispatch = log_dispatch.level(LevelFilter::Debug)
    } else {
        log_dispatch = log_dispatch.level(LevelFilter::Info);
    }

    if SERVER_DATA_MANAGER.read().settings().logging.log_to_disk {
        log_dispatch = log_dispatch.chain(
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(FILESYSTEM_LAYOUT.session_log())
                .unwrap(),
        );
    } else {
        // this sink is required to make sure all log gets processed and forwarded to the websocket
        if cfg!(target_os = "linux") {
            log_dispatch =
                log_dispatch.chain(OpenOptions::new().write(true).open("/dev/null").unwrap());
        } else {
            log_dispatch = log_dispatch.chain(std::io::stdout());
        }
    }

    log_dispatch
        .chain(
            Dispatch::new()
                .level(LevelFilter::Error)
                .chain(fern::log_file(FILESYSTEM_LAYOUT.crash_log()).unwrap()),
        )
        .apply()
        .unwrap();

    alvr_common::set_panic_hook();
}
