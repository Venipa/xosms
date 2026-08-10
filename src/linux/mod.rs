mod dbus;

use std::{
  sync::{Arc, RwLock},
  time::{Duration, Instant},
};

use ::dbus::{
  arg::{PropMap, Variant},
  blocking::stdintf::org_freedesktop_dbus::{EmitsChangedSignal, PropertiesPropertiesChanged},
  message::SignalArgs,
  MethodErr, Path,
};
use dashmap::DashMap;
use dbus_crossroads::Crossroads;
use float_duration::FloatDuration;
use napi::{
  bindgen_prelude::ObjectFinalize,
  threadsafe_function::{ErrorStrategy, ThreadsafeFunction, ThreadsafeFunctionCallMode},
  Env, JsFunction, NapiRaw,
};

use self::dbus::{
  mediaplayer2::{register_org_mpris_media_player2, OrgMprisMediaPlayer2},
  mediaplayer2_player::{
    register_org_mpris_media_player2_player, OrgMprisMediaPlayer2Player,
    OrgMprisMediaPlayer2PlayerSeeked,
  },
  session::{sanitize_mpris_instance_name, DBusSession},
};

const MPRIS_OBJECT_PATH: &str = "/org/mpris/MediaPlayer2";
const NO_TRACK_PATH: &str = "/org/mpris/MediaPlayer2/TrackList/NoTrack";
/// MPRIS MinimumRate must be > 0.0. Advertise the range we accept for Rate.
const MIN_PLAYBACK_RATE: f64 = 0.25;
const MAX_PLAYBACK_RATE: f64 = 2.0;

type ButtonListeners = Arc<DashMap<usize, ThreadsafeFunction<String, ErrorStrategy::CalleeHandled>>>;
type PositionListeners = Arc<DashMap<usize, ThreadsafeFunction<f64, ErrorStrategy::CalleeHandled>>>;

#[napi]
#[derive(Debug, PartialEq, Eq)]
pub enum MediaPlayerThumbnailType {
  Unknown = -1,
  File = 1,
  Uri = 2,
}

#[napi]
#[derive(Debug, PartialEq, Eq)]
pub enum MediaPlayerMediaType {
  Unknown = -1,
  Music = 1,
}

#[napi]
#[derive(Debug, PartialEq, Eq)]
pub enum MediaPlayerPlaybackStatus {
  Unknown = -1,
  Playing = 1,
  Paused = 2,
  Stopped = 3,
}

#[napi]
struct MediaPlayerThumbnail {
  thumbnail_type: MediaPlayerThumbnailType,
  thumbnail: String,
}

#[napi]
impl MediaPlayerThumbnail {
  #[napi(factory)]
  #[allow(dead_code)]
  pub async fn create(
    thumbnail_type: MediaPlayerThumbnailType,
    thumbnail: String,
  ) -> napi::Result<Self> {
    match thumbnail_type {
      MediaPlayerThumbnailType::File => Ok(Self {
        thumbnail_type,
        thumbnail: format!("file://{}", thumbnail),
      }),
      MediaPlayerThumbnailType::Uri => Ok(Self {
        thumbnail_type,
        thumbnail,
      }),
      _ => Err(napi::Error::from_reason(format!(
        "{:?} is not a valid MediaPlayerThumbnailType to create",
        thumbnail_type
      ))),
    }
  }

  #[napi(getter, js_name = "type")]
  #[allow(dead_code)]
  pub fn thumbnail_type(&self) -> MediaPlayerThumbnailType {
    self.thumbnail_type
  }
}

struct MprisPlayerState {
  identity: String,
  can_go_next: bool,
  can_go_previous: bool,
  can_play: bool,
  can_pause: bool,
  can_seek: bool,
  can_control: bool,
  media_type: MediaPlayerMediaType,
  playback_status: MediaPlayerPlaybackStatus,
  thumbnail: String,
  artist: String,
  album_title: String,
  title: String,
  track_id: String,
  position: f64,
  last_updated_position: Instant,
  duration: f64,
  volume: f64,
  playback_rate: f64,
}

impl MprisPlayerState {
  fn new(identity: String) -> Self {
    Self {
      identity,
      can_go_next: false,
      can_go_previous: false,
      can_play: false,
      can_pause: false,
      can_seek: false,
      can_control: true,
      media_type: MediaPlayerMediaType::Unknown,
      playback_status: MediaPlayerPlaybackStatus::Unknown,
      thumbnail: String::new(),
      artist: String::new(),
      album_title: String::new(),
      title: String::new(),
      track_id: String::new(),
      position: 0.0,
      last_updated_position: Instant::now(),
      duration: 0.0,
      volume: 1.0,
      playback_rate: 1.0,
    }
  }

  fn track_object_path(&self) -> Path<'static> {
    match sanitize_track_object_path(&self.track_id) {
      Some(path) => path,
      None => Path::from(NO_TRACK_PATH.to_string()),
    }
  }

  fn playback_status_label(&self) -> &'static str {
    match self.playback_status {
      MediaPlayerPlaybackStatus::Playing => "Playing",
      MediaPlayerPlaybackStatus::Paused => "Paused",
      _ => "Stopped",
    }
  }

  fn metadata_map(&self) -> PropMap {
    let mut metadata = PropMap::new();
    metadata.insert(
      "mpris:trackid".to_string(),
      Variant(Box::new(self.track_object_path())),
    );
    metadata.insert(
      "mpris:length".to_string(),
      Variant(Box::new(seconds_to_micros(self.duration))),
    );
    metadata.insert(
      "mpris:artUrl".to_string(),
      Variant(Box::new(self.thumbnail.clone())),
    );
    metadata.insert(
      "xesam:title".to_string(),
      Variant(Box::new(self.title.clone())),
    );
    metadata.insert(
      "xesam:album".to_string(),
      Variant(Box::new(self.album_title.clone())),
    );
    // MPRIS requires xesam:artist as an array of strings (`as`).
    metadata.insert(
      "xesam:artist".to_string(),
      Variant(Box::new(vec![self.artist.clone()])),
    );
    metadata
  }

  fn should_emit_seeked(&self, next_position: f64) -> bool {
    next_position - self.position > self.playback_rate
      && self.last_updated_position.elapsed().as_secs() < 1
  }
}

#[napi(custom_finalize)]
struct MediaPlayer {
  service_name: String,
  button_pressed_listeners: ButtonListeners,
  playback_position_changed_listeners: PositionListeners,
  playback_position_seeked_listeners: PositionListeners,
  player_state: Arc<RwLock<MprisPlayerState>>,
  properties_changed: PropertiesPropertiesChanged,
  active: bool,
  dbus_session: DBusSession,
}

#[napi]
impl MediaPlayer {
  #[napi(constructor)]
  #[allow(dead_code)]
  pub fn new(service_name: String, identity: String) -> napi::Result<Self> {
    let service_name = sanitize_mpris_instance_name(&service_name)
      .map_err(napi::Error::from_reason)?;

    Ok(Self {
      service_name,
      button_pressed_listeners: Arc::new(DashMap::new()),
      playback_position_changed_listeners: Arc::new(DashMap::new()),
      playback_position_seeked_listeners: Arc::new(DashMap::new()),
      player_state: Arc::new(RwLock::new(MprisPlayerState::new(identity))),
      properties_changed: PropertiesPropertiesChanged {
        interface_name: "org.mpris.MediaPlayer2.Player".to_string(),
        changed_properties: Default::default(),
        invalidated_properties: vec![],
      },
      active: false,
      dbus_session: DBusSession::new(),
    })
  }

  /// Activates the MediaPlayer allowing the operating system to see and use it
  #[napi]
  #[allow(dead_code)]
  pub fn activate(&mut self) -> napi::Result<()> {
    if self.active {
      return Ok(());
    }

    let mut crossroads = Crossroads::new();
    let mpris_iface_token = register_org_mpris_media_player2(&mut crossroads);
    let mpris_player_iface_token = register_org_mpris_media_player2_player(&mut crossroads);

    crossroads.insert(
      MPRIS_OBJECT_PATH,
      &[mpris_iface_token, mpris_player_iface_token],
      MprisPlayer {
        button_pressed_listeners: self.button_pressed_listeners.clone(),
        playback_position_changed_listeners: self.playback_position_changed_listeners.clone(),
        playback_position_seeked_listeners: self.playback_position_seeked_listeners.clone(),
        state: self.player_state.clone(),
      },
    );

    if !self.dbus_session.register(&self.service_name, crossroads) {
      return Err(napi::Error::from_reason(
        "Could not obtain service name on D-Bus",
      ));
    }

    self.active = true;
    self.queue_initial_properties();
    self.emit_properties_changed();
    Ok(())
  }

  /// Deactivates the MediaPlayer denying the operating system to see and use it
  #[napi]
  #[allow(dead_code)]
  pub fn deactivate(&mut self) -> napi::Result<()> {
    if self.active {
      self.active = false;
      self.dbus_session.unregister(&self.service_name);
    }
    Ok(())
  }

  /// Adds an event listener to the MediaPlayer
  ///
  /// 'buttonpressed' - Emitted when a media services button is pressed
  /// 'positionchanged' - Emitted when the media service requests a position change
  /// 'positionseeked' - Emitted when the media service requests a forward or backward position seek from current position
  #[napi]
  #[allow(dead_code)]
  pub fn add_event_listener(
    &mut self,
    env: Env,
    #[napi(ts_arg_type = "'buttonpressed' | 'positionchanged' | 'positionseeked'")]
    event_name: String,
    callback: JsFunction,
  ) -> napi::Result<()> {
    let callback_ptr = unsafe { callback.raw() as usize };

    match event_name.as_str() {
      "buttonpressed" => {
        insert_string_listener(&self.button_pressed_listeners, env, callback_ptr, callback)?
      }
      "positionchanged" => insert_f64_listener(
        &self.playback_position_changed_listeners,
        env,
        callback_ptr,
        callback,
      )?,
      "positionseeked" => insert_f64_listener(
        &self.playback_position_seeked_listeners,
        env,
        callback_ptr,
        callback,
      )?,
      _ => {}
    }

    Ok(())
  }

  /// Removes an event listener from the MediaPlayer
  #[napi]
  #[allow(dead_code)]
  pub fn remove_event_listener(
    &mut self,
    #[napi(ts_arg_type = "'buttonpressed' | 'positionchanged' | 'positionseeked'")]
    event_name: String,
    callback: JsFunction,
  ) -> napi::Result<()> {
    let callback_ptr = unsafe { callback.raw() as usize };

    match event_name.as_str() {
      "buttonpressed" => {
        self.button_pressed_listeners.remove(&callback_ptr);
      }
      "positionchanged" => {
        self
          .playback_position_changed_listeners
          .remove(&callback_ptr);
      }
      "positionseeked" => {
        self.playback_position_seeked_listeners.remove(&callback_ptr);
      }
      _ => {}
    }

    Ok(())
  }

  /// Adds an event listener to the MediaPlayer
  ///
  /// Alias for addEventListener
  #[napi]
  #[allow(dead_code)]
  pub fn on(
    &mut self,
    env: Env,
    #[napi(ts_arg_type = "'buttonpressed' | 'positionchanged' | 'positionseeked'")]
    event_name: String,
    callback: JsFunction,
  ) -> napi::Result<()> {
    self.add_event_listener(env, event_name, callback)
  }

  /// Removes an event listener from the MediaPlayer
  ///
  /// Alias for removeEventListener
  #[napi]
  #[allow(dead_code)]
  pub fn off(
    &mut self,
    #[napi(ts_arg_type = "'buttonpressed' | 'positionchanged' | 'positionseeked'")]
    event_name: String,
    callback: JsFunction,
  ) -> napi::Result<()> {
    self.remove_event_listener(event_name, callback)
  }

  /// Instructs the media service to update its media information being displayed
  ///
  /// On Linux (MPRIS), property setters only queue `PropertiesChanged`. Call
  /// `update()` after changing status, metadata, buttons, or rate so playerctl
  /// and desktop shells see the new values. `activate()` already flushes once.
  #[napi]
  #[allow(dead_code)]
  pub fn update(&mut self) -> napi::Result<()> {
    self.emit_properties_changed();
    Ok(())
  }

  /// Sets the thumbnail
  #[napi]
  #[allow(dead_code)]
  pub fn set_thumbnail(&mut self, thumbnail: &MediaPlayerThumbnail) -> napi::Result<()> {
    self.with_state_mut(|state| {
      state.thumbnail = thumbnail.thumbnail.clone();
    });
    self.queue_metadata_changed();
    Ok(())
  }

  /// Sets the timeline data
  ///
  /// You MUST call this function everytime the position changes in the song. The media service will become out of sync if this is not called enough or cause seeked signals to be emitted to the media service unnecessarily.
  #[napi]
  #[allow(dead_code)]
  pub fn set_timeline(&mut self, duration: f64, position: f64) -> napi::Result<()> {
    if duration < 0.0 {
      return Err(napi::Error::from_reason("Duration cannot be less than 0"));
    }
    if position < 0.0 {
      return Err(napi::Error::from_reason("Position cannot be less than 0"));
    }
    if position > duration {
      return Err(napi::Error::from_reason(
        "Position cannot be greather than provided duration",
      ));
    }

    let emit_seeked = self
      .player_state
      .write()
      .map(|mut state| {
        let should_seek = state.should_emit_seeked(position);
        state.duration = duration;
        state.position = position;
        state.last_updated_position = Instant::now();
        should_seek
      })
      .unwrap_or(false);

    if emit_seeked {
      let seeked = OrgMprisMediaPlayer2PlayerSeeked {
        position: seconds_to_micros(position),
      };
      self
        .dbus_session
        .emit_message(seeked.to_emit_message(&mpris_path()));
    }

    self.queue_metadata_changed();
    Ok(())
  }

  /// Gets the play button enbled state
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_play_button_enabled(&self) -> napi::Result<bool> {
    Ok(self.read_state(|state| state.can_play, false))
  }

  /// Sets the play button enbled state
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_play_button_enabled(&mut self, enabled: bool) -> napi::Result<()> {
    self.with_state_mut(|state| state.can_play = enabled);
    self.queue_bool_prop("CanPlay", enabled);
    Ok(())
  }

  /// Gets the paused button enbled state
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_pause_button_enabled(&self) -> napi::Result<bool> {
    Ok(self.read_state(|state| state.can_pause, false))
  }

  /// Sets the paused button enbled state
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_pause_button_enabled(&mut self, enabled: bool) -> napi::Result<()> {
    self.with_state_mut(|state| state.can_pause = enabled);
    self.queue_bool_prop("CanPause", enabled);
    Ok(())
  }

  /// Gets the paused button enbled state
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_stop_button_enabled(&self) -> napi::Result<bool> {
    // Stop button for MPRIS is tied to CanControl
    Ok(self.read_state(|state| state.can_control, false))
  }

  /// Sets the paused button enbled state
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_stop_button_enabled(&mut self, _enabled: bool) -> napi::Result<()> {
    // Stop button for MPRIS is tied to CanControl
    Ok(())
  }

  /// Gets the previous button enbled state
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_previous_button_enabled(&self) -> napi::Result<bool> {
    Ok(self.read_state(|state| state.can_go_previous, false))
  }

  /// Sets the previous button enbled state
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_previous_button_enabled(&mut self, enabled: bool) -> napi::Result<()> {
    self.with_state_mut(|state| state.can_go_previous = enabled);
    self.queue_bool_prop("CanGoPrevious", enabled);
    Ok(())
  }

  /// Gets the next button enbled state
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_next_button_enabled(&self) -> napi::Result<bool> {
    Ok(self.read_state(|state| state.can_go_next, false))
  }

  /// Sets the next button enbled state
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_next_button_enabled(&mut self, enabled: bool) -> napi::Result<()> {
    self.with_state_mut(|state| state.can_go_next = enabled);
    self.queue_bool_prop("CanGoNext", enabled);
    Ok(())
  }

  /// Gets the seek enabled state
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_seek_enabled(&self) -> napi::Result<bool> {
    Ok(self.read_state(|state| state.can_seek, false))
  }

  /// Sets the seek enbled state
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_seek_enabled(&mut self, enabled: bool) -> napi::Result<()> {
    self.with_state_mut(|state| state.can_seek = enabled);
    self.queue_bool_prop("CanSeek", enabled);
    Ok(())
  }

  /// Gets the playback rate
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_playback_rate(&self) -> napi::Result<f64> {
    Ok(self.read_state(|state| state.playback_rate, 1.0))
  }

  /// Sets the playback rate
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_playback_rate(&mut self, playback_rate: f64) -> napi::Result<()> {
    if !(MIN_PLAYBACK_RATE..=MAX_PLAYBACK_RATE).contains(&playback_rate) {
      return Err(napi::Error::from_reason(format!(
        "playbackRate must be between {MIN_PLAYBACK_RATE} and {MAX_PLAYBACK_RATE}"
      )));
    }

    self.with_state_mut(|state| state.playback_rate = playback_rate);
    self
      .properties_changed
      .add_prop("Rate", EmitsChangedSignal::True, || Box::new(playback_rate));
    Ok(())
  }

  /// Gets the playback status
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_playback_status(&self) -> napi::Result<MediaPlayerPlaybackStatus> {
    Ok(self.read_state(
      |state| state.playback_status,
      MediaPlayerPlaybackStatus::Unknown,
    ))
  }

  /// Sets the playback status
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_playback_status(
    &mut self,
    playback_status: MediaPlayerPlaybackStatus,
  ) -> napi::Result<()> {
    if playback_status == MediaPlayerPlaybackStatus::Unknown {
      return Err(napi::Error::from_reason(format!(
        "{:?} is not a valid MediaPlayerPlaybackStatus to set",
        playback_status
      )));
    }

    self.with_state_mut(|state| state.playback_status = playback_status);
    let label = match playback_status {
      MediaPlayerPlaybackStatus::Playing => "Playing",
      MediaPlayerPlaybackStatus::Paused => "Paused",
      _ => "Stopped",
    };
    self
      .properties_changed
      .add_prop("PlaybackStatus", EmitsChangedSignal::True, || {
        Box::new(label.to_string())
      });
    Ok(())
  }

  /// Gets the media type
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_media_type(&self) -> napi::Result<MediaPlayerMediaType> {
    Ok(self.read_state(|state| state.media_type, MediaPlayerMediaType::Unknown))
  }

  /// Sets the media type
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_media_type(&mut self, media_type: MediaPlayerMediaType) -> napi::Result<()> {
    if media_type == MediaPlayerMediaType::Unknown {
      return Err(napi::Error::from_reason(format!(
        "{:?} is not a valid MediaPlayerMediaType to set",
        media_type
      )));
    }

    self.with_state_mut(|state| state.media_type = media_type);
    Ok(())
  }

  /// Gets the media title
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_title(&self) -> napi::Result<String> {
    Ok(self.read_state(|state| state.title.clone(), String::new()))
  }

  /// Sets the media title
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_title(&mut self, title: String) -> napi::Result<()> {
    self.with_state_mut(|state| state.title = title);
    self.queue_metadata_changed();
    Ok(())
  }

  /// Gets the media artist
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_artist(&self) -> napi::Result<String> {
    Ok(self.read_state(|state| state.artist.clone(), String::new()))
  }

  /// Sets the media artist
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_artist(&mut self, artist: String) -> napi::Result<()> {
    self.with_state_mut(|state| state.artist = artist);
    self.queue_metadata_changed();
    Ok(())
  }

  /// Gets the media album title
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_album_title(&self) -> napi::Result<String> {
    Ok(self.read_state(|state| state.album_title.clone(), String::new()))
  }

  /// Sets the media artist
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_album_title(&mut self, album_title: String) -> napi::Result<()> {
    self.with_state_mut(|state| state.album_title = album_title);
    self.queue_metadata_changed();
    Ok(())
  }

  /// Gets the track id
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_track_id(&self) -> napi::Result<String> {
    Ok(self.read_state(|state| state.track_id.clone(), String::new()))
  }

  /// Sets the track id
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_track_id(&mut self, track_id: String) -> napi::Result<()> {
    self.with_state_mut(|state| state.track_id = track_id);
    self.queue_metadata_changed();
    Ok(())
  }

  fn read_state<T>(&self, mapper: impl FnOnce(&MprisPlayerState) -> T, fallback: T) -> T {
    self
      .player_state
      .read()
      .map(|state| mapper(&state))
      .unwrap_or(fallback)
  }

  fn with_state_mut(&self, mapper: impl FnOnce(&mut MprisPlayerState)) {
    if let Ok(mut state) = self.player_state.write() {
      mapper(&mut state);
    }
  }

  fn queue_bool_prop(&mut self, name: &str, value: bool) {
    self
      .properties_changed
      .add_prop(name, EmitsChangedSignal::True, || Box::new(value));
  }

  fn queue_initial_properties(&mut self) {
    let Ok(state) = self.player_state.read() else {
      return;
    };

    let playback_status = state.playback_status_label().to_string();
    let rate = state.playback_rate;
    let metadata = state.metadata_map();
    let can_play = state.can_play;
    let can_pause = state.can_pause;
    let can_go_previous = state.can_go_previous;
    let can_go_next = state.can_go_next;
    let can_seek = state.can_seek;
    let can_control = state.can_control;
    drop(state);

    self
      .properties_changed
      .add_prop("PlaybackStatus", EmitsChangedSignal::True, || {
        Box::new(playback_status)
      });
    self
      .properties_changed
      .add_prop("Rate", EmitsChangedSignal::True, || Box::new(rate));
    self
      .properties_changed
      .add_prop("Metadata", EmitsChangedSignal::True, || Box::new(metadata));
    self.queue_bool_prop("CanPlay", can_play);
    self.queue_bool_prop("CanPause", can_pause);
    self.queue_bool_prop("CanGoPrevious", can_go_previous);
    self.queue_bool_prop("CanGoNext", can_go_next);
    self.queue_bool_prop("CanSeek", can_seek);
    self.queue_bool_prop("CanControl", can_control);
  }

  fn queue_metadata_changed(&mut self) {
    let metadata = self
      .player_state
      .read()
      .map(|state| Box::new(state.metadata_map()))
      .unwrap_or_else(|_| Box::new(PropMap::new()));
    self
      .properties_changed
      .add_prop("Metadata", EmitsChangedSignal::True, || metadata);
  }

  fn emit_properties_changed(&mut self) {
    self
      .dbus_session
      .emit_message(self.properties_changed.to_emit_message(&mpris_path()));
    self.properties_changed.changed_properties.clear();
    self.properties_changed.invalidated_properties.clear();
  }
}

impl ObjectFinalize for MediaPlayer {
  fn finalize(self, _env: napi::Env) -> napi::Result<()> {
    self.dbus_session.unregister(&self.service_name);
    Ok(())
  }
}

struct MprisPlayer {
  button_pressed_listeners: ButtonListeners,
  playback_position_changed_listeners: PositionListeners,
  playback_position_seeked_listeners: PositionListeners,
  state: Arc<RwLock<MprisPlayerState>>,
}

impl MprisPlayer {
  fn with_state<T>(&self, mapper: impl FnOnce(&MprisPlayerState) -> T) -> Result<T, MethodErr> {
    self
      .state
      .read()
      .map(|state| mapper(&state))
      .map_err(|_| MethodErr::failed("Failed to read media player state"))
  }

  fn emit_button(&self, button: &str) {
    emit_string(&self.button_pressed_listeners, button);
  }

  fn emit_button_if(&self, allowed: impl FnOnce(&MprisPlayerState) -> bool, button: &str) {
    let Ok(state) = self.state.read() else {
      return;
    };
    if allowed(&state) {
      self.emit_button(button);
    }
  }
}

impl OrgMprisMediaPlayer2 for MprisPlayer {
  fn raise(&mut self) -> Result<(), MethodErr> {
    Ok(())
  }

  fn quit(&mut self) -> Result<(), MethodErr> {
    Ok(())
  }

  fn can_quit(&self) -> Result<bool, MethodErr> {
    Ok(false)
  }

  fn fullscreen(&self) -> Result<bool, MethodErr> {
    Ok(false)
  }

  fn set_fullscreen(&self, _value: bool) -> Result<(), MethodErr> {
    Ok(())
  }

  fn can_set_fullscreen(&self) -> Result<bool, MethodErr> {
    Ok(false)
  }

  fn can_raise(&self) -> Result<bool, MethodErr> {
    Ok(false)
  }

  fn has_track_list(&self) -> Result<bool, MethodErr> {
    Ok(false)
  }

  fn identity(&self) -> Result<String, MethodErr> {
    self.with_state(|state| state.identity.clone())
  }

  fn desktop_entry(&self) -> Result<String, MethodErr> {
    // Optional; not registered on the interface. Safe default if ever re-added.
    Ok(String::new())
  }

  fn supported_uri_schemes(&self) -> Result<Vec<String>, MethodErr> {
    Ok(vec![])
  }

  fn supported_mime_types(&self) -> Result<Vec<String>, MethodErr> {
    Ok(vec![])
  }
}

impl OrgMprisMediaPlayer2Player for MprisPlayer {
  fn next(&mut self) -> Result<(), MethodErr> {
    self.emit_button_if(|state| state.can_go_next, "next");
    Ok(())
  }

  fn previous(&mut self) -> Result<(), MethodErr> {
    self.emit_button_if(|state| state.can_go_previous, "previous");
    Ok(())
  }

  fn pause(&mut self) -> Result<(), MethodErr> {
    self.emit_button_if(|state| state.can_pause, "pause");
    Ok(())
  }

  fn play_pause(&mut self) -> Result<(), MethodErr> {
    let can_toggle = self.with_state(|state| state.can_pause || state.can_play)?;
    if !can_toggle {
      return Err(MethodErr::failed(
        "This media player cannot play or pause",
      ));
    }
    self.emit_button("playpause");
    Ok(())
  }

  fn stop(&mut self) -> Result<(), MethodErr> {
    let can_control = self.with_state(|state| state.can_control)?;
    if !can_control {
      return Err(MethodErr::failed("This media player cannot be controlled"));
    }
    self.emit_button("stop");
    Ok(())
  }

  fn play(&mut self) -> Result<(), MethodErr> {
    self.emit_button_if(|state| state.can_play, "play");
    Ok(())
  }

  fn seek(&mut self, offset: i64) -> Result<(), MethodErr> {
    let can_seek = self.with_state(|state| state.can_seek)?;
    if !can_seek {
      return Ok(());
    }

    emit_f64(
      &self.playback_position_seeked_listeners,
      FloatDuration::microseconds(offset as f64).as_seconds(),
    );
    Ok(())
  }

  fn set_position(
    &mut self,
    track_id: Path<'static>,
    position: i64,
  ) -> Result<(), MethodErr> {
    let accepted = self.with_state(|state| {
      if !state.can_seek || position < 0 {
        return false;
      }

      let requested_seconds = Duration::from_micros(position as u64).as_secs_f64();
      if requested_seconds > state.duration {
        return false;
      }

      // Stale SetPosition for a previous track must be ignored.
      track_id == state.track_object_path()
    })?;

    if accepted {
      emit_f64(
        &self.playback_position_changed_listeners,
        Duration::from_micros(position as u64).as_secs_f64(),
      );
    }

    Ok(())
  }

  fn open_uri(&mut self, _uri: String) -> Result<(), MethodErr> {
    Err(MethodErr::failed("OpenUri is not supported"))
  }

  fn playback_status(&self) -> Result<String, MethodErr> {
    self.with_state(|state| state.playback_status_label().to_string())
  }

  fn loop_status(&self) -> Result<String, MethodErr> {
    // Optional; not registered on the interface. Safe default if ever re-added.
    Ok(String::from("None"))
  }

  fn set_loop_status(&self, _value: String) -> Result<(), MethodErr> {
    Ok(())
  }

  fn rate(&self) -> Result<f64, MethodErr> {
    self.with_state(|state| state.playback_rate)
  }

  fn set_rate(&self, value: f64) -> Result<(), MethodErr> {
    // MPRIS: Rate = 0.0 is equivalent to Pause.
    if value == 0.0 {
      let can_pause = self.with_state(|state| state.can_pause)?;
      if can_pause {
        self.emit_button("pause");
      }
      return Ok(());
    }

    if !(MIN_PLAYBACK_RATE..=MAX_PLAYBACK_RATE).contains(&value) {
      return Err(MethodErr::invalid_arg("Rate out of MinimumRate..MaximumRate"));
    }

    if let Ok(mut state) = self.state.write() {
      state.playback_rate = value;
    }
    Ok(())
  }

  fn shuffle(&self) -> Result<bool, MethodErr> {
    // Optional; not registered on the interface. Safe default if ever re-added.
    Ok(false)
  }

  fn set_shuffle(&self, _value: bool) -> Result<(), MethodErr> {
    Ok(())
  }

  fn metadata(&self) -> Result<PropMap, MethodErr> {
    self.with_state(|state| state.metadata_map())
  }

  fn volume(&self) -> Result<f64, MethodErr> {
    self.with_state(|state| state.volume)
  }

  fn set_volume(&self, value: f64) -> Result<(), MethodErr> {
    if let Ok(mut state) = self.state.write() {
      state.volume = value.clamp(0.0, 1.0);
      return Ok(());
    }

    Err(MethodErr::failed("An error occurred while writing Volume"))
  }

  fn position(&self) -> Result<i64, MethodErr> {
    self.with_state(|state| seconds_to_micros(state.position))
  }

  fn minimum_rate(&self) -> Result<f64, MethodErr> {
    Ok(MIN_PLAYBACK_RATE)
  }

  fn maximum_rate(&self) -> Result<f64, MethodErr> {
    Ok(MAX_PLAYBACK_RATE)
  }

  fn can_go_next(&self) -> Result<bool, MethodErr> {
    self.with_state(|state| state.can_go_next)
  }

  fn can_go_previous(&self) -> Result<bool, MethodErr> {
    self.with_state(|state| state.can_go_previous)
  }

  fn can_play(&self) -> Result<bool, MethodErr> {
    self.with_state(|state| state.can_play)
  }

  fn can_pause(&self) -> Result<bool, MethodErr> {
    self.with_state(|state| state.can_pause)
  }

  fn can_seek(&self) -> Result<bool, MethodErr> {
    self.with_state(|state| state.can_seek)
  }

  fn can_control(&self) -> Result<bool, MethodErr> {
    self.with_state(|state| state.can_control)
  }
}

fn mpris_path() -> Path<'static> {
  Path::new(MPRIS_OBJECT_PATH).expect("valid MPRIS object path")
}

/// Build a valid D-Bus object path for `mpris:trackid`.
///
/// Object path elements may only use `[A-Za-z0-9_]`. Raw media ids (YouTube,
/// etc.) often contain `-` and other chars; `Path::from` panics on those and
/// aborts the host process when Metadata is read on the D-Bus thread.
fn sanitize_track_object_path(track_id: &str) -> Option<Path<'static>> {
  if track_id.is_empty() {
    return None;
  }

  let mut element = String::with_capacity(track_id.len());
  for character in track_id.chars() {
    if character.is_ascii_alphanumeric() || character == '_' {
      element.push(character);
    } else {
      element.push('_');
    }
  }

  while element.contains("__") {
    element = element.replace("__", "_");
  }
  let element = element.trim_matches('_');
  if element.is_empty() {
    return None;
  }

  Path::new(format!("/xosms/trackid/{element}")).ok()
}

fn seconds_to_micros(seconds: f64) -> i64 {
  FloatDuration::seconds(seconds)
    .as_microseconds()
    .clamp(i64::MIN as f64, i64::MAX as f64)
    .round() as i64
}

fn insert_string_listener(
  listeners: &ButtonListeners,
  env: Env,
  callback_ptr: usize,
  callback: JsFunction,
) -> napi::Result<()> {
  if listeners.contains_key(&callback_ptr) {
    return Ok(());
  }

  let mut threadsafe_callback = callback.create_threadsafe_function(0, |ctx| {
    ctx.env.create_string_from_std(ctx.value).map(|v| vec![v])
  })?;
  let _ = threadsafe_callback.unref(&env)?;
  listeners.insert(callback_ptr, threadsafe_callback);
  Ok(())
}

fn insert_f64_listener(
  listeners: &PositionListeners,
  env: Env,
  callback_ptr: usize,
  callback: JsFunction,
) -> napi::Result<()> {
  if listeners.contains_key(&callback_ptr) {
    return Ok(());
  }

  let mut threadsafe_callback =
    callback.create_threadsafe_function(0, |ctx| ctx.env.create_double(ctx.value).map(|v| vec![v]))?;
  let _ = threadsafe_callback.unref(&env)?;
  listeners.insert(callback_ptr, threadsafe_callback);
  Ok(())
}

fn emit_string(listeners: &ButtonListeners, value: &str) {
  for listener in listeners.iter() {
    listener.call(
      Ok(value.to_string()),
      ThreadsafeFunctionCallMode::NonBlocking,
    );
  }
}

fn emit_f64(listeners: &PositionListeners, value: f64) {
  for listener in listeners.iter() {
    listener.call(Ok(value), ThreadsafeFunctionCallMode::NonBlocking);
  }
}

#[cfg(test)]
mod tests {
  use super::{seconds_to_micros, MediaPlayerPlaybackStatus, MprisPlayerState, NO_TRACK_PATH};

  #[test]
  fn empty_track_id_uses_mpris_no_track_path() {
    let state = MprisPlayerState::new(String::from("Test Player"));
    assert_eq!(state.track_object_path().to_string(), NO_TRACK_PATH);
  }

  #[test]
  fn metadata_uses_artist_array_and_object_path_track_id() {
    let mut state = MprisPlayerState::new(String::from("Test Player"));
    state.track_id = String::from("abc");
    state.artist = String::from("Artist");
    state.title = String::from("Title");
    state.duration = 12.5;

    let metadata = state.metadata_map();
    assert!(metadata.contains_key("mpris:trackid"));
    assert!(metadata.contains_key("xesam:artist"));
    assert_eq!(
      state.track_object_path().to_string(),
      "/xosms/trackid/abc"
    );
    assert_eq!(seconds_to_micros(12.5), 12_500_000);
  }

  #[test]
  fn track_id_with_hyphen_sanitizes_to_valid_object_path() {
    let mut state = MprisPlayerState::new(String::from("Test Player"));
    // YouTube-style ids include `-`, which is illegal in D-Bus object paths.
    state.track_id = String::from("dQw4w9WgXcQ-extra");
    assert_eq!(
      state.track_object_path().to_string(),
      "/xosms/trackid/dQw4w9WgXcQ_extra"
    );
  }

  #[test]
  fn track_id_with_only_invalid_chars_falls_back_to_no_track() {
    let mut state = MprisPlayerState::new(String::from("Test Player"));
    state.track_id = String::from("---");
    assert_eq!(state.track_object_path().to_string(), NO_TRACK_PATH);
  }

  #[test]
  fn seeked_emits_only_on_large_position_jumps() {
    let mut state = MprisPlayerState::new(String::from("Test Player"));
    state.position = 10.0;
    state.playback_rate = 1.0;
    assert!(state.should_emit_seeked(12.0));
    assert!(!state.should_emit_seeked(10.5));
  }

  #[test]
  fn playback_status_label_maps_unknown_to_stopped() {
    let mut state = MprisPlayerState::new(String::from("Test Player"));
    state.playback_status = MediaPlayerPlaybackStatus::Unknown;
    assert_eq!(state.playback_status_label(), "Stopped");
    state.playback_status = MediaPlayerPlaybackStatus::Playing;
    assert_eq!(state.playback_status_label(), "Playing");
  }
}
