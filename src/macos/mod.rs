use std::{
  ffi::c_void,
  sync::{Arc, RwLock},
  time::Duration,
};

use dashmap::DashMap;
use napi::{
  bindgen_prelude::ObjectFinalize,
  threadsafe_function::{ErrorStrategy, ThreadsafeFunction, ThreadsafeFunctionCallMode},
  Env, JsFunction, NapiRaw,
};
use souvlaki::{
  MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
  SeekDirection,
};

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

#[derive(Debug)]
struct MediaPlayerState {
  active: bool,
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
  duration: f64,
  position: f64,
  playback_rate: f64,
}

#[napi(custom_finalize)]
struct MediaPlayer {
  media_controls: MediaControls,
  button_pressed_listeners:
    Arc<DashMap<usize, ThreadsafeFunction<String, ErrorStrategy::CalleeHandled>>>,
  playback_position_changed_listeners:
    Arc<DashMap<usize, ThreadsafeFunction<f64, ErrorStrategy::CalleeHandled>>>,
  playback_position_seeked_listeners:
    Arc<DashMap<usize, ThreadsafeFunction<f64, ErrorStrategy::CalleeHandled>>>,
  state: Arc<RwLock<MediaPlayerState>>,
}

#[napi]
impl MediaPlayer {
  #[napi(constructor)]
  #[allow(dead_code)]
  pub fn new(service_name: String, identity: String) -> napi::Result<Self> {
    let button_pressed_listeners: Arc<
      DashMap<usize, ThreadsafeFunction<String, ErrorStrategy::CalleeHandled>>,
    > = Arc::new(DashMap::new());
    let playback_position_changed_listeners: Arc<
      DashMap<usize, ThreadsafeFunction<f64, ErrorStrategy::CalleeHandled>>,
    > = Arc::new(DashMap::new());
    let playback_position_seeked_listeners: Arc<
      DashMap<usize, ThreadsafeFunction<f64, ErrorStrategy::CalleeHandled>>,
    > = Arc::new(DashMap::new());

    let state: Arc<RwLock<MediaPlayerState>> = Arc::new(RwLock::new(MediaPlayerState {
      active: false,
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
      duration: 0.0,
      position: 0.0,
      playback_rate: 1.0,
    }));

    let mut media_controls: MediaControls = MediaControls::new(PlatformConfig {
      display_name: &identity,
      dbus_name: &service_name,
      hwnd: Option::<*mut c_void>::None,
    })
    .map_err(map_souvlaki_error)?;

    let closure_state: Arc<RwLock<MediaPlayerState>> = state.clone();
    let closure_button_pressed_listeners = button_pressed_listeners.clone();
    let closure_position_changed_listeners = playback_position_changed_listeners.clone();
    let closure_position_seeked_listeners = playback_position_seeked_listeners.clone();

    media_controls
      .attach(move |event: MediaControlEvent| {
        handle_media_control_event(
          event,
          &closure_state,
          &closure_button_pressed_listeners,
          &closure_position_changed_listeners,
          &closure_position_seeked_listeners,
        );
      })
      .map_err(map_souvlaki_error)?;

    Ok(Self {
      media_controls,
      button_pressed_listeners,
      playback_position_changed_listeners,
      playback_position_seeked_listeners,
      state,
    })
  }

  /// Activates the MediaPlayer allowing the operating system to see and use it
  #[napi]
  #[allow(dead_code)]
  pub fn activate(&mut self) -> napi::Result<()> {
    if let Ok(mut state) = self.state.write() {
      state.active = true;
    }
    self.publish_state()
  }

  /// Deactivates the MediaPlayer denying the operating system to see and use it
  #[napi]
  #[allow(dead_code)]
  pub fn deactivate(&mut self) -> napi::Result<()> {
    if let Ok(mut state) = self.state.write() {
      state.active = false;
    }
    self
      .media_controls
      .set_playback(MediaPlayback::Stopped)
      .map_err(map_souvlaki_error)
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
    let callback_ptr: usize = unsafe { callback.raw() as usize };

    match event_name.as_str() {
      "buttonpressed" => {
        if !self.button_pressed_listeners.contains_key(&callback_ptr) {
          let mut threadsafe_callback = callback.create_threadsafe_function(0, |ctx| {
            ctx.env.create_string_from_std(ctx.value).map(|v| vec![v])
          })?;
          let _ = threadsafe_callback.unref(&env)?;
          self
            .button_pressed_listeners
            .insert(callback_ptr, threadsafe_callback);
        }
      }
      "positionchanged" => {
        if !self
          .playback_position_changed_listeners
          .contains_key(&callback_ptr)
        {
          let mut threadsafe_callback = callback.create_threadsafe_function(0, |ctx| {
            ctx.env.create_double(ctx.value).map(|v| vec![v])
          })?;
          let _ = threadsafe_callback.unref(&env)?;
          self
            .playback_position_changed_listeners
            .insert(callback_ptr, threadsafe_callback);
        }
      }
      "positionseeked" => {
        if !self
          .playback_position_seeked_listeners
          .contains_key(&callback_ptr)
        {
          let mut threadsafe_callback = callback.create_threadsafe_function(0, |ctx| {
            ctx.env.create_double(ctx.value).map(|v| vec![v])
          })?;
          let _ = threadsafe_callback.unref(&env)?;
          self
            .playback_position_seeked_listeners
            .insert(callback_ptr, threadsafe_callback);
        }
      }
      _ => {}
    };

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
    let callback_ptr: usize = unsafe { callback.raw() as usize };

    match event_name.as_str() {
      "buttonpressed" => {
        if self.button_pressed_listeners.contains_key(&callback_ptr) {
          self.button_pressed_listeners.remove(&callback_ptr);
        }
      }
      "positionchanged" => {
        if self
          .playback_position_changed_listeners
          .contains_key(&callback_ptr)
        {
          self
            .playback_position_changed_listeners
            .remove(&callback_ptr);
        }
      }
      "positionseeked" => {
        if self
          .playback_position_seeked_listeners
          .contains_key(&callback_ptr)
        {
          self
            .playback_position_seeked_listeners
            .remove(&callback_ptr);
        }
      }
      _ => {}
    };

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
  #[napi]
  #[allow(dead_code)]
  pub fn update(&mut self) -> napi::Result<()> {
    self.publish_state()
  }

  /// Sets the thumbnail
  #[napi]
  #[allow(dead_code)]
  pub fn set_thumbnail(&mut self, thumbnail: &MediaPlayerThumbnail) -> napi::Result<()> {
    if let Ok(mut state) = self.state.write() {
      state.thumbnail = thumbnail.thumbnail.to_owned();
    }
    self.update_metadata()
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

    if let Ok(mut state) = self.state.write() {
      state.duration = duration;
      state.position = position;
    }

    self.publish_state()
  }

  /// Gets the play button enbled state
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_play_button_enabled(&self) -> napi::Result<bool> {
    if let Ok(state) = self.state.read() {
      return Ok(state.can_play);
    }

    Ok(false)
  }

  /// Sets the play button enbled state
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_play_button_enabled(&mut self, enabled: bool) -> napi::Result<()> {
    if let Ok(mut state) = self.state.write() {
      state.can_play = enabled;
    }
    Ok(())
  }

  /// Gets the paused button enbled state
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_pause_button_enabled(&self) -> napi::Result<bool> {
    if let Ok(state) = self.state.read() {
      return Ok(state.can_pause);
    }

    Ok(false)
  }

  /// Sets the paused button enbled state
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_pause_button_enabled(&mut self, enabled: bool) -> napi::Result<()> {
    if let Ok(mut state) = self.state.write() {
      state.can_pause = enabled;
    }
    Ok(())
  }

  /// Gets the paused button enbled state
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_stop_button_enabled(&self) -> napi::Result<bool> {
    if let Ok(state) = self.state.read() {
      return Ok(state.can_control);
    }

    Ok(false)
  }

  /// Sets the paused button enbled state
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_stop_button_enabled(&mut self, enabled: bool) -> napi::Result<()> {
    if let Ok(mut state) = self.state.write() {
      state.can_control = enabled;
    }
    Ok(())
  }

  /// Gets the previous button enbled state
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_previous_button_enabled(&self) -> napi::Result<bool> {
    if let Ok(state) = self.state.read() {
      return Ok(state.can_go_previous);
    }

    Ok(false)
  }

  /// Sets the previous button enbled state
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_previous_button_enabled(&mut self, enabled: bool) -> napi::Result<()> {
    if let Ok(mut state) = self.state.write() {
      state.can_go_previous = enabled;
    }
    Ok(())
  }

  /// Gets the next button enbled state
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_next_button_enabled(&self) -> napi::Result<bool> {
    if let Ok(state) = self.state.read() {
      return Ok(state.can_go_next);
    }

    Ok(false)
  }

  /// Sets the next button enbled state
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_next_button_enabled(&mut self, enabled: bool) -> napi::Result<()> {
    if let Ok(mut state) = self.state.write() {
      state.can_go_next = enabled;
    }
    Ok(())
  }

  /// Gets the seek enabled state
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_seek_enabled(&self) -> napi::Result<bool> {
    if let Ok(state) = self.state.read() {
      return Ok(state.can_seek);
    }

    Ok(false)
  }

  /// Sets the seek enabled state
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_seek_enabled(&mut self, enabled: bool) -> napi::Result<()> {
    if let Ok(mut state) = self.state.write() {
      state.can_seek = enabled;
    }
    Ok(())
  }

  /// Gets the playback rate
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_playback_rate(&self) -> napi::Result<f64> {
    if let Ok(state) = self.state.read() {
      return Ok(state.playback_rate);
    }

    Ok(1.0)
  }

  /// Sets the playback rate
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_playback_rate(&mut self, playback_rate: f64) -> napi::Result<()> {
    if let Ok(mut state) = self.state.write() {
      state.playback_rate = playback_rate;
    }
    Ok(())
  }

  /// Gets the playback status
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_playback_status(&self) -> napi::Result<MediaPlayerPlaybackStatus> {
    if let Ok(state) = self.state.read() {
      return Ok(state.playback_status);
    }

    Ok(MediaPlayerPlaybackStatus::Unknown)
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

    if let Ok(mut state) = self.state.write() {
      state.playback_status = playback_status;
    }

    self.update_playback()
  }

  /// Gets the media type
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_media_type(&self) -> napi::Result<MediaPlayerMediaType> {
    if let Ok(state) = self.state.read() {
      return Ok(state.media_type);
    }

    Ok(MediaPlayerMediaType::Unknown)
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

    if let Ok(mut state) = self.state.write() {
      state.media_type = media_type;
    }

    Ok(())
  }

  /// Gets the media title
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_title(&self) -> napi::Result<String> {
    if let Ok(state) = self.state.read() {
      return Ok(state.title.to_owned());
    }

    Ok(String::new())
  }

  /// Sets the media title
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_title(&mut self, title: String) -> napi::Result<()> {
    if let Ok(mut state) = self.state.write() {
      state.title = title;
    }

    self.update_metadata()
  }

  /// Gets the media artist
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_artist(&self) -> napi::Result<String> {
    if let Ok(state) = self.state.read() {
      return Ok(state.artist.to_owned());
    }

    Ok(String::new())
  }

  /// Sets the media artist
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_artist(&mut self, artist: String) -> napi::Result<()> {
    if let Ok(mut state) = self.state.write() {
      state.artist = artist;
    }

    self.update_metadata()
  }

  /// Gets the media album title
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_album_title(&self) -> napi::Result<String> {
    if let Ok(state) = self.state.read() {
      return Ok(state.album_title.to_owned());
    }

    Ok(String::new())
  }

  /// Sets the media artist
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_album_title(&mut self, album_title: String) -> napi::Result<()> {
    if let Ok(mut state) = self.state.write() {
      state.album_title = album_title;
    }

    self.update_metadata()
  }

  /// Gets the track id
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_track_id(&self) -> napi::Result<String> {
    if let Ok(state) = self.state.read() {
      return Ok(state.track_id.to_owned());
    }

    Ok(String::new())
  }

  /// Sets the track id
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_track_id(&mut self, track_id: String) -> napi::Result<()> {
    if let Ok(mut state) = self.state.write() {
      state.track_id = track_id;
    }

    Ok(())
  }

  fn publish_state(&mut self) -> napi::Result<()> {
    self.update_metadata()?;
    self.update_playback()
  }

  fn update_metadata(&mut self) -> napi::Result<()> {
    if let Ok(state) = self.state.read() {
      if !state.active {
        return Ok(());
      }

      let metadata: MediaMetadata<'_> = MediaMetadata {
        title: to_optional_ref(state.title.as_str()),
        album: to_optional_ref(state.album_title.as_str()),
        artist: to_optional_ref(state.artist.as_str()),
        cover_url: to_optional_ref(state.thumbnail.as_str()),
        duration: Some(Duration::from_secs_f64(state.duration.max(0.0))),
      };

      return self
        .media_controls
        .set_metadata(metadata)
        .map_err(map_souvlaki_error);
    }

    Ok(())
  }

  fn update_playback(&mut self) -> napi::Result<()> {
    if let Ok(state) = self.state.read() {
      if !state.active {
        return Ok(());
      }

      let progress: Option<MediaPosition> = Some(MediaPosition(Duration::from_secs_f64(
        state.position.max(0.0),
      )));
      let playback: MediaPlayback = match state.playback_status {
        MediaPlayerPlaybackStatus::Playing => MediaPlayback::Playing { progress },
        MediaPlayerPlaybackStatus::Paused => MediaPlayback::Paused { progress },
        _ => MediaPlayback::Stopped,
      };

      return self
        .media_controls
        .set_playback(playback)
        .map_err(map_souvlaki_error);
    }

    Ok(())
  }
}

impl ObjectFinalize for MediaPlayer {
  fn finalize(mut self, _env: napi::Env) -> napi::Result<()> {
    let _ = self.media_controls.detach();
    self.button_pressed_listeners.clear();
    self.playback_position_changed_listeners.clear();
    self.playback_position_seeked_listeners.clear();
    Ok(())
  }
}

fn handle_media_control_event(
  event: MediaControlEvent,
  state: &Arc<RwLock<MediaPlayerState>>,
  button_pressed_listeners: &Arc<
    DashMap<usize, ThreadsafeFunction<String, ErrorStrategy::CalleeHandled>>,
  >,
  playback_position_changed_listeners: &Arc<
    DashMap<usize, ThreadsafeFunction<f64, ErrorStrategy::CalleeHandled>>,
  >,
  playback_position_seeked_listeners: &Arc<
    DashMap<usize, ThreadsafeFunction<f64, ErrorStrategy::CalleeHandled>>,
  >,
) {
  let Ok(current_state) = state.read() else {
    return;
  };

  if !current_state.active {
    return;
  }

  match event {
    MediaControlEvent::Play if current_state.can_play => {
      emit_button_pressed(button_pressed_listeners, "play");
    }
    MediaControlEvent::Pause if current_state.can_pause => {
      emit_button_pressed(button_pressed_listeners, "pause");
    }
    MediaControlEvent::Toggle if current_state.can_play || current_state.can_pause => {
      emit_button_pressed(button_pressed_listeners, "playpause");
    }
    MediaControlEvent::Next if current_state.can_go_next => {
      emit_button_pressed(button_pressed_listeners, "next");
    }
    MediaControlEvent::Previous if current_state.can_go_previous => {
      emit_button_pressed(button_pressed_listeners, "previous");
    }
    MediaControlEvent::Stop if current_state.can_control => {
      emit_button_pressed(button_pressed_listeners, "stop");
    }
    MediaControlEvent::SeekBy(direction, amount) if current_state.can_seek => {
      let signed_seconds: f64 = match direction {
        SeekDirection::Forward => amount.as_secs_f64(),
        SeekDirection::Backward => -amount.as_secs_f64(),
      };
      emit_seek(playback_position_seeked_listeners, signed_seconds);
    }
    MediaControlEvent::SetPosition(position) if current_state.can_seek => {
      let requested_seconds: f64 = position.0.as_secs_f64();
      if requested_seconds <= current_state.duration {
        emit_position(playback_position_changed_listeners, requested_seconds);
      }
    }
    _ => {}
  }
}

fn emit_button_pressed(
  listeners: &Arc<DashMap<usize, ThreadsafeFunction<String, ErrorStrategy::CalleeHandled>>>,
  button: &str,
) {
  for listener in listeners.iter() {
    listener.call(
      Ok(button.to_string()),
      ThreadsafeFunctionCallMode::NonBlocking,
    );
  }
}

fn emit_position(
  listeners: &Arc<DashMap<usize, ThreadsafeFunction<f64, ErrorStrategy::CalleeHandled>>>,
  position: f64,
) {
  for listener in listeners.iter() {
    listener.call(Ok(position), ThreadsafeFunctionCallMode::NonBlocking);
  }
}

fn emit_seek(
  listeners: &Arc<DashMap<usize, ThreadsafeFunction<f64, ErrorStrategy::CalleeHandled>>>,
  seek_delta: f64,
) {
  for listener in listeners.iter() {
    listener.call(Ok(seek_delta), ThreadsafeFunctionCallMode::NonBlocking);
  }
}

fn to_optional_ref(value: &str) -> Option<&str> {
  if value.is_empty() {
    None
  } else {
    Some(value)
  }
}

fn map_souvlaki_error(error: souvlaki::Error) -> napi::Error {
  napi::Error::from_reason(format!("{:?}", error))
}

