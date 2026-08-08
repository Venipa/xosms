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

struct RegisteredPlayer {
  bus_name: String,
  crossroads: Crossroads,
}

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
      let mut registered: Option<RegisteredPlayer> = None;

      loop {
        let Ok(connection) = Connection::new_session() else {
          thread::sleep(RECONNECT_DELAY);
          continue;
        };

        // Re-claim well-known name after reconnect so playerctl still sees us.
        if let Some(player) = registered.as_ref() {
          if !claim_bus_name(&connection, &player.bus_name) {
            thread::sleep(RECONNECT_DELAY);
            continue;
          }
        }

        loop {
          if let Ok((name, crossroads, response)) = register_name_receiver.try_recv() {
            let claimed = claim_bus_name(&connection, &name);
            if claimed {
              registered = Some(RegisteredPlayer {
                bus_name: name,
                crossroads,
              });
            }
            let _ = response.send(claimed);
          }

          if let Ok((name, response)) = unregister_name_receiver.try_recv() {
            let released = if registered
              .as_ref()
              .is_some_and(|player| player.bus_name == name)
            {
              registered = None;
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
            // Keep registered player so the next connection can reclaim the bus name.
            break;
          }

          while let Some(message) = connection.channel().pop_message() {
            if let Some(player) = registered.as_mut() {
              let _ = player.crossroads.handle_message(message, &connection);
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

  pub fn register(&self, name: &str, crossroads: Crossroads) -> bool {
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

  pub fn unregister(&self, name: &str) -> bool {
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

fn claim_bus_name(connection: &Connection, name: &str) -> bool {
  connection
    .request_name(name, false, true, true)
    .ok()
    .is_some_and(|reply| reply == RequestNameReply::PrimaryOwner)
}

fn mpris_bus_name(service_name: &str) -> String {
  format!("org.mpris.MediaPlayer2.{}", service_name)
}

/// Sanitize a user-provided MPRIS instance name into a valid D-Bus name element.
///
/// D-Bus well-known name elements must match `[A-Za-z_][A-Za-z0-9_-]*`.
pub fn sanitize_mpris_instance_name(raw: &str) -> Result<String, String> {
  let mut sanitized: String = raw
    .chars()
    .map(|character| {
      if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
        character
      } else {
        '_'
      }
    })
    .collect();

  while sanitized.contains("__") {
    sanitized = sanitized.replace("__", "_");
  }
  let sanitized = sanitized
    .trim_matches(|character| character == '_' || character == '-')
    .to_string();

  if sanitized.is_empty() {
    return Err(String::from(
      "serviceName must contain at least one alphanumeric character for MPRIS D-Bus registration",
    ));
  }

  let starts_with_digit = sanitized
    .chars()
    .next()
    .is_some_and(|character| character.is_ascii_digit());
  if starts_with_digit {
    return Ok(format!("p{sanitized}"));
  }

  Ok(sanitized)
}

#[cfg(test)]
mod tests {
  use super::sanitize_mpris_instance_name;

  #[test]
  fn sanitizes_spaces_and_symbols() {
    assert_eq!(
      sanitize_mpris_instance_name("My App!").unwrap(),
      "My_App"
    );
  }

  #[test]
  fn prefixes_leading_digits() {
    assert_eq!(sanitize_mpris_instance_name("1player").unwrap(), "p1player");
  }

  #[test]
  fn rejects_empty_after_sanitize() {
    assert!(sanitize_mpris_instance_name("!!!").is_err());
  }
}
