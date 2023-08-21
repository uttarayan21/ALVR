use crate::{
    bindings::FfiButtonValue, connection::ClientDisconnectRequest, DECODER_CONFIG,
    DISCONNECT_CLIENT_NOTIFIER, EVENTS_BUS, FILESYSTEM_LAYOUT, SERVER_DATA_MANAGER, SHUTTING_DOWN,
    VIDEO_MIRROR_BUS, VIDEO_RECORDING_FILE,
};
use alvr_common::{
    anyhow::{anyhow, Result},
    error, info, log,
};
use alvr_events::{ButtonEvent, EventType};
use alvr_packets::{ButtonValue, ClientListAction, ServerRequest};
use alvr_session::ConnectionState;
use bus::Bus;
use rouille::{try_or_400, try_or_404, websocket, Response};
use std::{
    env,
    fs::File,
    sync::mpsc::Sender,
    thread::{self, JoinHandle},
};

pub const WS_BROADCAST_CAPACITY: usize = 256;

pub type WebserverHandles = (JoinHandle<()>, Sender<()>);

pub fn webserver() -> Result<WebserverHandles> {
    let web_server_port = SERVER_DATA_MANAGER
        .read()
        .settings()
        .connection
        .web_server_port;

    let server = rouille::Server::new(("0.0.0.0", web_server_port), move |request| {
        if SHUTTING_DOWN.value() {
            // Service not available
            return Response::text("Server is shutting down").with_status_code(503);
        }
        match request.url().as_str() {
            "/api/dashboard-request" => {
                let Some(body) = request.data() else {
                    return Response::empty_400();
                };

                match try_or_400!(serde_json::from_reader(body)) {
                    ServerRequest::Log(event) => {
                        let level = event.severity.into_log_level();
                        log::log!(level, "{}", event.content);
                    }
                    ServerRequest::GetSession => {
                        alvr_events::send_event(EventType::Session(Box::new(
                            SERVER_DATA_MANAGER.read().session().clone(),
                        )));
                    }
                    ServerRequest::UpdateSession(session) => {
                        *SERVER_DATA_MANAGER.write().session_mut() = *session
                    }
                    ServerRequest::SetValues(descs) => {
                        SERVER_DATA_MANAGER.write().set_values(descs).ok();
                    }
                    ServerRequest::UpdateClientList { hostname, action } => {
                        let mut data_manager = SERVER_DATA_MANAGER.write();
                        if matches!(action, ClientListAction::RemoveEntry) {
                            if let Some(entry) = data_manager.client_list().get(&hostname) {
                                if entry.connection_state != ConnectionState::Disconnected {
                                    data_manager.update_client_list(
                                        hostname.clone(),
                                        ClientListAction::SetConnectionState(
                                            ConnectionState::Disconnecting {
                                                should_be_removed: true,
                                            },
                                        ),
                                    );
                                } else {
                                    data_manager.update_client_list(hostname, action);
                                }
                            }
                        } else {
                            data_manager.update_client_list(hostname, action);
                        }

                        if let Some(notifier) = &*DISCONNECT_CLIENT_NOTIFIER.lock() {
                            notifier.send(ClientDisconnectRequest::Disconnect).ok();
                        }
                    }
                    ServerRequest::GetAudioDevices => {
                        if let Ok(list) = SERVER_DATA_MANAGER.read().get_audio_devices_list() {
                            alvr_events::send_event(EventType::AudioDevices(list));
                        }
                    }
                    ServerRequest::CaptureFrame => unsafe { crate::CaptureFrame() },
                    ServerRequest::InsertIdr => unsafe { crate::RequestIDR() },
                    ServerRequest::StartRecording => crate::create_recording_file(),
                    ServerRequest::StopRecording => *VIDEO_RECORDING_FILE.lock() = None,
                    ServerRequest::FirewallRules(action) => {
                        if alvr_server_io::firewall_rules(action).is_ok() {
                            info!("Setting firewall rules succeeded!");
                        } else {
                            error!("Setting firewall rules failed!");
                        }
                    }
                    ServerRequest::RegisterAlvrDriver => {
                        alvr_server_io::driver_registration(
                            &[FILESYSTEM_LAYOUT.openvr_driver_root_dir.clone()],
                            true,
                        )
                        .ok();

                        if let Ok(list) = alvr_server_io::get_registered_drivers() {
                            alvr_events::send_event(EventType::DriversList(list));
                        }
                    }
                    ServerRequest::UnregisterDriver(path) => {
                        alvr_server_io::driver_registration(&[path], false).ok();

                        if let Ok(list) = alvr_server_io::get_registered_drivers() {
                            alvr_events::send_event(EventType::DriversList(list));
                        }
                    }
                    ServerRequest::GetDriverList => {
                        if let Ok(list) = alvr_server_io::get_registered_drivers() {
                            alvr_events::send_event(EventType::DriversList(list));
                        }
                    }
                    ServerRequest::RestartSteamvr => {
                        thread::spawn(crate::restart_driver);
                    }
                    ServerRequest::ShutdownSteamvr => {
                        // This lint is bugged with extern "C"
                        #[allow(clippy::redundant_closure)]
                        thread::spawn(|| crate::shutdown_driver());
                    }
                }

                Response::empty_204()
            }
            "/api/events" => {
                error!("subscribing to events!");
                let (response, future_websocket) =
                    try_or_400!(websocket::start::<&str>(request, None));

                thread::spawn(move || {
                    if let Ok(mut websocket) = future_websocket.recv() {
                        let mut receiver = if let Some(bus) = &mut *EVENTS_BUS.lock() {
                            bus.add_rx()
                        } else {
                            return;
                        };
                        let mut iter = receiver.iter();

                        while !SHUTTING_DOWN.value() {
                            if let Some(data) = iter.next() {
                                if let Err(e) =
                                    websocket.send_text(&serde_json::to_string(&data).unwrap())
                                {
                                    info!("Failed to send log with websocket: {e:?}");
                                    break;
                                }
                            }
                        }
                        error!("finished events websocket!");
                    }
                });

                response
            }
            "/api/video-mirror" => {
                let (response, future_websocket) =
                    try_or_400!(websocket::start::<&str>(request, None));

                thread::spawn({
                    move || {
                        if let Ok(mut websocket) = future_websocket.recv() {
                            let mut receiver = {
                                let mut bus_lock = VIDEO_MIRROR_BUS.lock();
                                let bus = bus_lock.insert(Bus::new(WS_BROADCAST_CAPACITY));

                                let receiver = bus.add_rx();

                                if let Some(config) = &*DECODER_CONFIG.lock() {
                                    bus.try_broadcast(config.config_buffer.clone()).ok();
                                }

                                receiver
                            };

                            unsafe { crate::RequestIDR() };

                            let mut iter = receiver.iter();

                            while !SHUTTING_DOWN.value() {
                                if let Some(data) = iter.next() {
                                    if let Err(e) = websocket.send_binary(&data) {
                                        info!("Failed to send video packet with websocket: {e:?}");
                                        break;
                                    }
                                }
                            }
                            error!("websocket finished!");
                        }
                    }
                });

                response
            }
            "/api/set-buttons" => {
                let Some(body) = request.data() else {
                    return Response::empty_400();
                };

                for button in try_or_400!(serde_json::from_reader::<_, Vec<ButtonEvent>>(body)) {
                    let value = match button.value {
                        ButtonValue::Binary(value) => FfiButtonValue {
                            type_: crate::FfiButtonType_BUTTON_TYPE_BINARY,
                            __bindgen_anon_1: crate::FfiButtonValue__bindgen_ty_1 {
                                binary: value.into(),
                            },
                        },

                        ButtonValue::Scalar(value) => FfiButtonValue {
                            type_: crate::FfiButtonType_BUTTON_TYPE_SCALAR,
                            __bindgen_anon_1: crate::FfiButtonValue__bindgen_ty_1 { scalar: value },
                        },
                    };

                    unsafe { crate::SetButton(alvr_common::hash_string(&button.path), value) };
                }

                Response::empty_204()
            }
            "/api/ping" => Response::empty_204(),
            mut other_uri => {
                if other_uri == "/" {
                    other_uri = "/index.html";
                }

                let content_type = if other_uri.ends_with(".html") {
                    "text/html"
                } else if other_uri.ends_with(".js") {
                    "text/javascript"
                } else if other_uri.ends_with(".wasm") {
                    "application/wasm"
                } else {
                    "text/plain"
                };

                let file = try_or_404!(File::open(format!(
                    "{}/ui{other_uri}",
                    env::current_dir().unwrap().to_string_lossy(),
                )));

                Response::from_file(content_type, file)
            }
        }
    })
    .map_err(|e| anyhow!("{e}"))?;

    Ok(server.stoppable())
}
