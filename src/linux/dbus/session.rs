use std::{
  sync::mpsc,
  thread::{self, JoinHandle},
  time::Duration,
};

use dbus::{
  blocking::{
    stdintf::org_freedesktop_dbus::{ReleaseNameReply, RequestNameReply},
    Connection,
  },
  channel::Sender,
  Message,
};
use dbus_crossroads::Crossroads;

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const RECONNECT_DELAY: Duration = Duration::from_millis(250);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(1);

type RegisterRequest = (String, Crossroads, oneshot::Sender<bool>);
type UnregisterRequest = (String, oneshot::Sender<bool>);

pub struct DBusSession {
  _dbus_connection_handle: JoinHandle<()>,
  register_name: mpsc::Sender<RegisterRequest>,
  unregister_name: mpsc::Sender<UnregisterRequest>,
  emit_message: mpsc::Sender<Message>,
}

impl DBusSession {
  pub fn new() -> Self {
    let (register_name, register_name_receiver) = mpsc::channel::<RegisterRequest>();
    let (unregister_name, unregister_name_receiver) = mpsc::channel::<UnregisterRequest>();
    let (emit_message, emit_message_receiver) = mpsc::channel::<Message>();

    let dbus_connection_handle = thread::spawn(move || {
      let mut media_player: Option<Crossroads> = None;

      loop {
        let Ok(connection) = Connection::new_session() else {
          thread::sleep(RECONNECT_DELAY);
          continue;
        };

        loop {
          if let Ok((name, crossroads, response)) = register_name_receiver.try_recv() {
            let registered = connection
              .request_name(&name, false, true, true)
              .ok()
              .is_some_and(|reply| reply == RequestNameReply::PrimaryOwner);

            if registered {
              media_player = Some(crossroads);
            }
            let _ = response.send(registered);
          }

          if let Ok((name, response)) = unregister_name_receiver.try_recv() {
            let released = if media_player.take().is_some() {
              connection
                .release_name(&name)
                .ok()
                .is_some_and(|reply| reply == ReleaseNameReply::Released)
            } else {
              false
            };
            let _ = response.send(released);
          }

          while let Ok(message) = emit_message_receiver.try_recv() {
            let _ = connection.send(message);
          }

          if connection.channel().read_write(Some(POLL_INTERVAL)).is_err() {
            media_player = None;
            break;
          }

          while let Some(message) = connection.channel().pop_message() {
            if let Some(crossroads) = media_player.as_mut() {
              let _ = crossroads.handle_message(message, &connection);
            }
          }
        }
      }
    });

    Self {
      _dbus_connection_handle: dbus_connection_handle,
      register_name,
      unregister_name,
      emit_message,
    }
  }

  pub fn register(&self, name: &String, crossroads: Crossroads) -> bool {
    let bus_name = mpris_bus_name(name);
    let (response_sender, response_receiver) = oneshot::channel();
    if self
      .register_name
      .send((bus_name, crossroads, response_sender))
      .is_err()
    {
      return false;
    }

    response_receiver
      .recv_timeout(REQUEST_TIMEOUT)
      .unwrap_or(false)
  }

  pub fn unregister(&self, name: &String) -> bool {
    let bus_name = mpris_bus_name(name);
    let (response_sender, response_receiver) = oneshot::channel();
    if self
      .unregister_name
      .send((bus_name, response_sender))
      .is_err()
    {
      return false;
    }

    response_receiver
      .recv_timeout(REQUEST_TIMEOUT)
      .unwrap_or(false)
  }

  pub fn emit_message(&self, message: Message) {
    let _ = self.emit_message.send(message);
  }
}

fn mpris_bus_name(service_name: &str) -> String {
  format!("org.mpris.MediaPlayer2.{}", service_name)
}
